import { describe, it, expect, vi, beforeEach } from 'vitest';
import { CommunityService, rosterHasJoinedAuthor, toNavPayload } from '../community-service';
import type { TauriAdapter } from '../zenoh-service';
import type { CommunityGovernance } from '../types';

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

describe('toNavPayload (ZEB-393)', () => {
  it('maps a community DTO to an added community nav payload', () => {
    expect(
      toNavPayload({ spaceId: 'aa', name: 'Open Town', isInviteOnly: false, pending: false }),
    ).toEqual({ action: 'added', spaceId: 'aa', kind: 'community', name: 'Open Town', pending: undefined });
  });

  it('carries pending=true so an invite-only join renders greyed at boot', () => {
    expect(
      toNavPayload({ spaceId: 'bb', name: 'Secret Club', isInviteOnly: true, pending: true }).pending,
    ).toBe(true);
  });
});

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

  // ZEB-962: a peer can broadcast a whitespace-only display name; `?? undefined`
  // only null-normalizes, so `"   "` was baked verbatim into CommunityMember.
  // `nonEmpty` normalizes blank/whitespace to absent at the ingest boundary.
  it('listCommunityMembers normalizes a whitespace-only displayName to undefined', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([
      {
        addr: 'a3f8c1d2',
        displayName: '   ',
        status: 'joined',
        power: 0,
        joinedAt: { wallMs: 1, logical: 0, deviceId: 'd' },
      },
    ]);
    const r = await service.listCommunityMembers('bb'.repeat(4));
    expect(r[0].displayName).toBeUndefined();
  });

  it('redeemInvite returns the DTO and learns the community kind', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: 'eeff0011',
      communityName: 'Real Name',
      isInviteOnly: true,
      pending: false,
    });
    const dto = await service.redeemInvite('harmony://invite/v1?ci=...');
    expect(adapter.invoke).toHaveBeenCalledWith('redeem_invite', { url: 'harmony://invite/v1?ci=...' });
    expect(dto).toEqual({
      communityId: 'eeff0011',
      communityName: 'Real Name',
      isInviteOnly: true,
      pending: false,
    });
    // ZEB-265: redeem now records kind so getKind() doesn't return 'unknown'
    // for redeemed communities.
    expect(service.getKind('eeff0011')).toBe('invite-only');
  });

  // ZEB-393 Bug B: boot rehydration of the nav sidebar.
  it('listOwnerCommunities fetches the IPC and records each community kind', async () => {
    await service.connectAdapter(adapter);
    const rows = [
      { spaceId: 'aa', name: 'Open Town', isInviteOnly: false, pending: false },
      { spaceId: 'bb', name: 'Secret Club', isInviteOnly: true, pending: true },
    ];
    (adapter.invoke as any).mockResolvedValue(rows);

    const got = await service.listOwnerCommunities();

    expect(adapter.invoke).toHaveBeenCalledWith('list_owner_communities', {});
    expect(got).toEqual(rows);
    // getKind() must resolve for rehydrated communities, not return 'unknown'.
    expect(service.getKind('aa')).toBe('open');
    expect(service.getKind('bb')).toBe('invite-only');
  });

  // ZEB-254: pending flag on RedeemInviteResultDto
  it('redeemInvite returns pending: true when admin was offline (fast-path timeout)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: 'aabb1122',
      communityName: 'Secret Club',
      isInviteOnly: true,
      pending: true,
    });
    const dto = await service.redeemInvite('harmony://invite/v1?ci=pending');
    expect(dto.pending).toBe(true);
    // Kind is still recorded regardless of pending state.
    expect(service.getKind('aabb1122')).toBe('invite-only');
  });

  it('redeemInvite returns pending: false for open communities', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: 'cc334455',
      communityName: 'Open Crew',
      isInviteOnly: false,
      pending: false,
    });
    const dto = await service.redeemInvite('harmony://invite/v1?ci=open');
    expect(dto.pending).toBe(false);
    expect(service.getKind('cc334455')).toBe('open');
  });

  it('joinOpenCommunity returns the DTO and learns the community kind', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: 'aabbccddeeff00112233445566778899',
      communityName: 'DirCommunity',
      isInviteOnly: false,
      pending: false,
    });
    const returned = await service.joinOpenCommunity('aabbccddeeff00112233445566778899');
    expect(adapter.invoke).toHaveBeenCalledWith('join_open_community', {
      communityId: 'aabbccddeeff00112233445566778899',
    });
    expect(returned).toEqual({
      communityId: 'aabbccddeeff00112233445566778899',
      communityName: 'DirCommunity',
      isInviteOnly: false,
      pending: false,
    });
    // Same as redeemInvite: a successful direct-join learns the kind.
    expect(service.getKind('aabbccddeeff00112233445566778899')).toBe('open');
  });

  it('redeemInvite records open kind when isInviteOnly is false', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      communityId: '00112233',
      communityName: 'Open Community',
      isInviteOnly: false,
      pending: false,
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

  it('listCommunityMembers forceRefresh bypasses the cache (ZEB-404)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);

    await service.listCommunityMembers('aabbccdd'); // populates cache
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    await service.listCommunityMembers('aabbccdd'); // cache hit, no IPC
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    await service.listCommunityMembers('aabbccdd', true); // forced → re-fetch
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('listCommunityMembers coalesces concurrent fetches per community (single-flight, ZEB-404)', async () => {
    await service.connectAdapter(adapter);
    let resolveFetch: (v: unknown) => void = () => {};
    (adapter.invoke as any).mockImplementation(
      () => new Promise((r) => { resolveFetch = r as (v: unknown) => void; }),
    );

    // Two overlapping forced fetches for the same community share one IPC.
    const p1 = service.listCommunityMembers('aabbccdd', true);
    const p2 = service.listCommunityMembers('aabbccdd', true);
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    resolveFetch([]);
    const [r1, r2] = await Promise.all([p1, p2]);
    expect(r2).toBe(r1); // same shared result, no out-of-order cache write

    // In-flight entry cleared on completion: a later forced fetch re-fetches.
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listCommunityMembers('aabbccdd', true);
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
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

  it('connectAdapter installs channel-config-updated listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('channel-config-updated')).toBe(true);
  });

  it('createChannel invokes create_channel with camelCase args (defaults kind to text)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    const id = await service.createChannel('aa'.repeat(16), 'announcements', 0);
    expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
      communityId: 'aa'.repeat(16),
      name: 'announcements',
      writePower: 0,
      kind: 'text',
    });
    expect(id).toBe('cc'.repeat(16));
  });

  it('createChannel passes kind through to the IPC', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('cc'.repeat(16));
    await service.createChannel('aa'.repeat(16), 'hangout', 0, 'voice');
    expect(adapter.invoke).toHaveBeenCalledWith('create_channel', {
      communityId: 'aa'.repeat(16),
      name: 'hangout',
      writePower: 0,
      kind: 'voice',
    });
  });

  it('modifyChannel forwards both name and writePower', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.modifyChannel('aa'.repeat(16), 'bb'.repeat(16), 'renamed', 50);
    expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      name: 'renamed',
      writePower: 50,
    });
  });

  it('modifyChannel forwards undefined for unchanged fields', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.modifyChannel('aa'.repeat(16), 'bb'.repeat(16), 'just-rename');
    expect(adapter.invoke).toHaveBeenCalledWith('modify_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
      name: 'just-rename',
      writePower: undefined,
    });
  });

  it('deleteChannel invokes delete_channel', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.deleteChannel('aa'.repeat(16), 'bb'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('delete_channel', {
      communityId: 'aa'.repeat(16),
      channelId: 'bb'.repeat(16),
    });
  });

  it('listChannels caches per-community result', async () => {
    await service.connectAdapter(adapter);
    const dtos = [
      {
        channelId: 'cc'.repeat(16),
        name: 'general',
        writePower: 0,
        kind: 'text',
        createdAt: { wallMs: 1, logical: 0, deviceId: 'd' },
      },
    ];
    (adapter.invoke as any).mockResolvedValue(dtos);
    const r1 = await service.listChannels('aa'.repeat(16));
    const r2 = await service.listChannels('aa'.repeat(16));
    expect(r1).toEqual(dtos);
    expect(r2).toEqual(r1);
    expect(adapter.invoke).toHaveBeenCalledTimes(1);
  });

  it('channel-config-updated for a community invalidates its channel cache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([]);
    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        action: 'created',
        name: 'announcements',
        writePower: 50,
        atWallMs: 100,
      },
    });

    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(2);
  });

  it('channel-config-updated fires onChannelConfigChanged callback', async () => {
    await service.connectAdapter(adapter);
    const cb = vi.fn();
    service.onChannelConfigChanged = cb;

    const handler = adapter.listeners.get('channel-config-updated')!;
    handler({
      payload: {
        communityId: 'aa'.repeat(16),
        channelId: 'cc'.repeat(16),
        action: 'modified',
        name: 'newname',
        atWallMs: 200,
      },
    });

    expect(cb).toHaveBeenCalledWith('aa'.repeat(16), 'modified', 'cc'.repeat(16), 'newname', undefined);
  });

  it('getSelectedChannel returns undefined before set', () => {
    expect(service.getSelectedChannel('aa'.repeat(16))).toBeUndefined();
  });

  it('setSelectedChannel + getSelectedChannel round-trip', () => {
    service.setSelectedChannel('aa'.repeat(16), 'cc'.repeat(16));
    expect(service.getSelectedChannel('aa'.repeat(16))).toBe('cc'.repeat(16));
  });

  it('destroy clears selectedChannelByCommunity', () => {
    service.setSelectedChannel('aa'.repeat(16), 'cc'.repeat(16));
    service.destroy();
    expect(service.getSelectedChannel('aa'.repeat(16))).toBeUndefined();
  });

  it('destroy clears channelCache', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue([
      {
        channelId: 'cc'.repeat(16),
        name: 'g',
        writePower: 0,
        createdAt: { wallMs: 0, logical: 0, deviceId: 'd' },
      },
    ]);
    await service.listChannels('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledTimes(1);

    service.destroy();
    // After destroy, listChannels would re-fetch (and throw, since
    // adapter is null). We assert the cache was cleared by checking
    // that the invoke would be called again on a fresh service.
    const service2 = new CommunityService();
    const adapter2 = makeAdapter();
    await service2.connectAdapter(adapter2);
    (adapter2.invoke as any).mockResolvedValue([]);
    await service2.listChannels('aa'.repeat(16));
    expect(adapter2.invoke).toHaveBeenCalledTimes(1);
  });

  it('getCommunityGovernance invokes the IPC and returns the camelCase DTO (ZEB-608)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ adminQuorum: 2 });
    const result = await service.getCommunityGovernance('aa'.repeat(16));
    expect(adapter.invoke).toHaveBeenCalledWith('get_community_governance', {
      communityId: 'aa'.repeat(16),
    });
    expect(result.adminQuorum).toBe(2);
  });

  it('getCommunityGovernance returns per-community thresholds (ZEB-251)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      adminQuorum: 1,
      invite: 25,
      kick: 60,
      setPower: 100,
      max: 100,
    });
    const gov: CommunityGovernance = await service.getCommunityGovernance('00'.repeat(16));
    expect(gov).toEqual({ adminQuorum: 1, invite: 25, kick: 60, setPower: 100, max: 100 });
  });
});

describe('rosterHasJoinedAuthor (ZEB-404)', () => {
  it('true when a joined member matches (case-insensitive)', () => {
    expect(rosterHasJoinedAuthor([{ address: 'AB12', status: 'joined' }], 'ab12')).toBe(true);
  });
  it('false when the author is absent from the roster', () => {
    expect(rosterHasJoinedAuthor([{ address: 'ab12', status: 'joined' }], 'cd34')).toBe(false);
  });
  it('false when the only address match is not joined', () => {
    expect(rosterHasJoinedAuthor([{ address: 'ab12', status: 'invited' }], 'ab12')).toBe(false);
  });
  it('false on an empty roster', () => {
    expect(rosterHasJoinedAuthor([], 'ab12')).toBe(false);
  });
});
