import { describe, it, expect, vi, beforeEach } from 'vitest';
import { FriendService, type FriendDto } from './friend-service';
import type { TauriAdapter } from './zenoh-service';

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

describe('FriendService', () => {
  let service: FriendService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new FriendService();
    adapter = makeAdapter();
  });

  it('connectAdapter installs the friend-list-changed listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('friend-list-changed')).toBe(true);
  });

  it('generateFriendToken invokes generate_friend_token with expiresAt: null when omitted', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('harmony://friend/AAAA');
    const url = await service.generateFriendToken();
    expect(adapter.invoke).toHaveBeenCalledWith('generate_friend_token', { expiresAt: null });
    expect(url).toBe('harmony://friend/AAAA');
  });

  it('generateFriendToken forwards a provided expiresAt', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('harmony://friend/BBBB');
    await service.generateFriendToken(1_700_000_000_000);
    expect(adapter.invoke).toHaveBeenCalledWith('generate_friend_token', {
      expiresAt: 1_700_000_000_000,
    });
  });

  it('redeemFriendToken invokes redeem_friend_token with the url and returns the DTO', async () => {
    await service.connectAdapter(adapter);
    const result = { ownerIdHex: 'aabbccdd00112233aabbccdd00112233', display: 'Alice' };
    (adapter.invoke as any).mockResolvedValue(result);
    const dto = await service.redeemFriendToken('harmony://friend/CCCC');
    expect(adapter.invoke).toHaveBeenCalledWith('redeem_friend_token', {
      url: 'harmony://friend/CCCC',
    });
    expect(dto).toEqual(result);
  });

  it('listFriends invokes list_friends and returns the DTO array', async () => {
    await service.connectAdapter(adapter);
    const friends: FriendDto[] = [
      {
        ownerIdHex: '11223344556677889900aabbccddeeff',
        display: 'Bob',
        status: 'active',
        establishedVia: 'token',
        referrable: false,
      },
    ];
    (adapter.invoke as any).mockResolvedValue(friends);
    const r = await service.listFriends();
    expect(adapter.invoke).toHaveBeenCalledWith('list_friends', {});
    expect(r).toEqual(friends);
  });

  it('unfriend invokes unfriend with the peerAddr (camelCased) param', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.unfriend('deadbeefdeadbeefdeadbeefdeadbeef');
    expect(adapter.invoke).toHaveBeenCalledWith('unfriend', {
      peerAddr: 'deadbeefdeadbeefdeadbeefdeadbeef',
    });
  });

  it('a friend-list-changed event fires onFriendsChanged', async () => {
    const changed = vi.fn();
    service.onFriendsChanged = changed;
    await service.connectAdapter(adapter);
    // Simulate the backend emitting the event.
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(changed).toHaveBeenCalledTimes(1);
  });

  it('invoke normalizes a thrown Error into a fresh Error message', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockRejectedValue('redeem failed: inviter_unreachable');
    await expect(service.redeemFriendToken('harmony://friend/X')).rejects.toThrow(
      'redeem failed: inviter_unreachable',
    );
  });

  it('throws when invoked before an adapter is connected', async () => {
    await expect(service.listFriends()).rejects.toThrow('adapter not connected');
  });
});
