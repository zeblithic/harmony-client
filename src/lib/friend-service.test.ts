import { describe, it, expect, vi, beforeEach } from 'vitest';
import {
  FriendService,
  contactsFromFriends,
  type FriendDto,
  type AddFriendOutcome,
} from './friend-service';
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

  it('generateFriendToken invokes generate_friend_token with ttlMs: null when omitted', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('harmony://friend/AAAA');
    const url = await service.generateFriendToken();
    expect(adapter.invoke).toHaveBeenCalledWith('generate_friend_token', { ttlMs: null });
    expect(url).toBe('harmony://friend/AAAA');
  });

  it('generateFriendToken forwards a provided ttlMs (a duration, not an epoch)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('harmony://friend/BBBB');
    const fourHoursMs = 4 * 60 * 60 * 1000;
    await service.generateFriendToken(fourHoursMs);
    expect(adapter.invoke).toHaveBeenCalledWith('generate_friend_token', {
      ttlMs: fourHoursMs,
    });
  });

  it('generateFriendToken preserves a zero ttlMs (?? not ||)', async () => {
    // 0 is a meaningful TTL: the backend resolves it to expires_at == now
    // (immediate expiry). The service uses `ttlMs ?? null`, so 0 must forward as
    // `ttlMs: 0`; switching to `|| null` would silently collapse it to "no
    // expiry". Mirrors the Rust `resolve_expiry_ms_zero_ttl_is_immediate` guard.
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('harmony://friend/CCCC');
    await service.generateFriendToken(0);
    expect(adapter.invoke).toHaveBeenCalledWith('generate_friend_token', { ttlMs: 0 });
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

  it('a friend-list-changed event fires all registered onFriendsChanged listeners', async () => {
    const first = vi.fn();
    const second = vi.fn();
    service.onFriendsChanged(first);
    service.onFriendsChanged(second);
    await service.connectAdapter(adapter);
    // Simulate the backend emitting the event.
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(1);
    // A second emission fires both again (idempotent registration).
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(first).toHaveBeenCalledTimes(2);
    expect(second).toHaveBeenCalledTimes(2);
  });

  it('an unsubscribed onFriendsChanged listener no longer fires', async () => {
    const stays = vi.fn();
    const leaves = vi.fn();
    service.onFriendsChanged(stays);
    const unsubscribe = service.onFriendsChanged(leaves);
    await service.connectAdapter(adapter);

    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(stays).toHaveBeenCalledTimes(1);
    expect(leaves).toHaveBeenCalledTimes(1);

    // Unsubscribe `leaves`; only `stays` should fire on the next emission.
    unsubscribe();
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(stays).toHaveBeenCalledTimes(2);
    expect(leaves).toHaveBeenCalledTimes(1);
  });

  it('destroy() clears all onFriendsChanged listeners', async () => {
    const changed = vi.fn();
    service.onFriendsChanged(changed);
    await service.connectAdapter(adapter);
    service.destroy();
    // The listener registry is cleared even if some event source survived.
    await service.connectAdapter(adapter);
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(changed).not.toHaveBeenCalled();
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

  // ── Phase 1b: pending requests, add-by-key, auto-accept ──────────────────

  it('listPendingRequests invokes list_pending_friend_requests and returns the array', async () => {
    await service.connectAdapter(adapter);
    const pending = [
      { ownerIdHex: 'aabbccdd00112233aabbccdd00112233', display: 'Alice', receivedAtMs: 1_700_000_000_000 },
    ];
    (adapter.invoke as any).mockResolvedValue(pending);
    const r = await service.listPendingRequests();
    expect(adapter.invoke).toHaveBeenCalledWith('list_pending_friend_requests', {});
    expect(r).toEqual(pending);
  });

  it('acceptRequest invokes accept_friend_request with the ownerIdHex arg', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.acceptRequest('deadbeefdeadbeefdeadbeefdeadbeef');
    expect(adapter.invoke).toHaveBeenCalledWith('accept_friend_request', {
      ownerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef',
    });
  });

  it('declineRequest invokes decline_friend_request with the ownerIdHex arg', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.declineRequest('deadbeefdeadbeefdeadbeefdeadbeef');
    expect(adapter.invoke).toHaveBeenCalledWith('decline_friend_request', {
      ownerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef',
    });
  });

  it('listOutboundRequests invokes list_outbound_friend_requests (ZEB-783)', async () => {
    await service.connectAdapter(adapter);
    const row = {
      identityPubHex: 'cd'.repeat(64),
      requestedAtMs: 1_700_000_000_000,
      expiresAtMs: 1_700_000_000_000 + 7 * 24 * 60 * 60 * 1000,
    };
    (adapter.invoke as any).mockResolvedValue([row]);
    const rows = await service.listOutboundRequests();
    expect(adapter.invoke).toHaveBeenCalledWith('list_outbound_friend_requests', {});
    expect(rows).toEqual([row]);
  });

  it('cancelOutboundRequest invokes cancel_outbound_friend_request with identityPubHex', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.cancelOutboundRequest('cd'.repeat(64));
    expect(adapter.invoke).toHaveBeenCalledWith('cancel_outbound_friend_request', {
      identityPubHex: 'cd'.repeat(64),
    });
  });

  it('addByKey invokes add_friend_by_key with identityPubHex and returns a linked outcome', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      linked: { ownerIdHex: 'aabbccdd00112233aabbccdd00112233', display: 'Alice' },
    });
    const result = await service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233');
    expect(adapter.invoke).toHaveBeenCalledWith('add_friend_by_key', {
      identityPubHex: 'aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233',
    });
    expect(result).toEqual<AddFriendOutcome>({
      kind: 'linked',
      ownerIdHex: 'aabbccdd00112233aabbccdd00112233',
      display: 'Alice',
    });
  });

  it('addByKey returns a pending outcome when the backend returns "pending"', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('pending');
    const result = await service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233');
    expect(result).toEqual<AddFriendOutcome>({ kind: 'pending' });
  });

  it('addByKey returns an unreachable outcome when the backend returns "unreachable"', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('unreachable');
    const result = await service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233');
    expect(result).toEqual<AddFriendOutcome>({ kind: 'unreachable' });
  });

  it('addByKey throws a descriptive error on an unknown backend outcome', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ unknown_variant: 'oops' });
    await expect(
      service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233'),
    ).rejects.toThrow('unexpected add_friend_by_key outcome:');
  });

  it('addByKey rejects a malformed linked payload (missing ownerIdHex)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ linked: {} });
    await expect(
      service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233'),
    ).rejects.toThrow('unexpected add_friend_by_key outcome:');
  });

  it('addByKey rejects a linked payload whose ownerIdHex is not a string', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({ linked: { ownerIdHex: 7 } });
    await expect(
      service.addByKey('aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233'),
    ).rejects.toThrow('unexpected add_friend_by_key outcome:');
  });

  it('addByKey accepts a linked payload with no display (→ null)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue({
      linked: { ownerIdHex: 'aabbccdd00112233aabbccdd00112233' },
    });
    const result = await service.addByKey(
      'aabbccdd00112233aabbccdd00112233aabbccdd00112233aabbccdd00112233',
    );
    expect(result).toEqual<AddFriendOutcome>({
      kind: 'linked',
      ownerIdHex: 'aabbccdd00112233aabbccdd00112233',
      display: null,
    });
  });

  it('getMyIdentityPubHex invokes connectivity_get_my_identity_pub_hex and returns the hex', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('ab'.repeat(64));
    const result = await service.getMyIdentityPubHex();
    expect(adapter.invoke).toHaveBeenCalledWith('connectivity_get_my_identity_pub_hex', {});
    expect(result).toBe('ab'.repeat(64));
  });

  it('getMyIdentityPubHex returns null when the node is not started', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(null);
    const result = await service.getMyIdentityPubHex();
    expect(result).toBeNull();
  });

  it('setAutoAccept invokes set_friend_auto_accept with enabled', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.setAutoAccept(true);
    expect(adapter.invoke).toHaveBeenCalledWith('set_friend_auto_accept', { enabled: true });
    await service.setAutoAccept(false);
    expect(adapter.invoke).toHaveBeenCalledWith('set_friend_auto_accept', { enabled: false });
  });

  it('setReferrable invokes set_friend_referrable with the ownerIdHex + referrable args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.setReferrable('aabb', true);
    expect(adapter.invoke).toHaveBeenCalledWith('set_friend_referrable', {
      ownerIdHex: 'aabb',
      referrable: true,
    });
    await service.setReferrable('ccdd', false);
    expect(adapter.invoke).toHaveBeenCalledWith('set_friend_referrable', {
      ownerIdHex: 'ccdd',
      referrable: false,
    });
  });

  it('setNickname invokes set_friend_nickname with ownerIdHex + nickname (null clears)', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.setNickname('aabb', 'Koya');
    expect(adapter.invoke).toHaveBeenCalledWith('set_friend_nickname', {
      ownerIdHex: 'aabb',
      nickname: 'Koya',
    });
    await service.setNickname('ccdd', null);
    expect(adapter.invoke).toHaveBeenLastCalledWith('set_friend_nickname', {
      ownerIdHex: 'ccdd',
      nickname: null,
    });
  });

  it('browseReferrals invokes browse_friend_referrals with the ownerIdHex arg and returns the array', async () => {
    await service.connectAdapter(adapter);
    const referrals = [
      { ownerIdHex: '11223344556677889900aabbccddeeff', display: 'Carol', alreadyFriend: false },
      { ownerIdHex: 'ffeeddccbbaa00998877665544332211', display: null, alreadyFriend: true },
    ];
    (adapter.invoke as any).mockResolvedValue(referrals);
    const r = await service.browseReferrals('aabb');
    expect(adapter.invoke).toHaveBeenCalledWith('browse_friend_referrals', { ownerIdHex: 'aabb' });
    expect(r).toEqual(referrals);
  });

  it('getAutoAccept invokes get_friend_auto_accept and returns the boolean', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(true);
    const result = await service.getAutoAccept();
    expect(adapter.invoke).toHaveBeenCalledWith('get_friend_auto_accept', {});
    expect(result).toBe(true);
  });

  it('connectAdapter installs the friend-request-received listener', async () => {
    await service.connectAdapter(adapter);
    expect(adapter.listeners.has('friend-request-received')).toBe(true);
  });

  it('a friend-request-received event fires all registered onPendingRequestsChanged listeners', async () => {
    const first = vi.fn();
    const second = vi.fn();
    service.onPendingRequestsChanged(first);
    service.onPendingRequestsChanged(second);
    await service.connectAdapter(adapter);
    adapter.listeners.get('friend-request-received')!({ payload: null });
    expect(first).toHaveBeenCalledTimes(1);
    expect(second).toHaveBeenCalledTimes(1);
    adapter.listeners.get('friend-request-received')!({ payload: null });
    expect(first).toHaveBeenCalledTimes(2);
    expect(second).toHaveBeenCalledTimes(2);
  });

  it('a friend-list-changed event also fires onPendingRequestsChanged listeners', async () => {
    const pendingCb = vi.fn();
    service.onPendingRequestsChanged(pendingCb);
    await service.connectAdapter(adapter);
    adapter.listeners.get('friend-list-changed')!({ payload: null });
    expect(pendingCb).toHaveBeenCalledTimes(1);
  });

  it('an unsubscribed onPendingRequestsChanged listener no longer fires', async () => {
    const stays = vi.fn();
    const leaves = vi.fn();
    service.onPendingRequestsChanged(stays);
    const unsubscribe = service.onPendingRequestsChanged(leaves);
    await service.connectAdapter(adapter);

    adapter.listeners.get('friend-request-received')!({ payload: null });
    expect(stays).toHaveBeenCalledTimes(1);
    expect(leaves).toHaveBeenCalledTimes(1);

    unsubscribe();
    adapter.listeners.get('friend-request-received')!({ payload: null });
    expect(stays).toHaveBeenCalledTimes(2);
    expect(leaves).toHaveBeenCalledTimes(1);
  });

  it('destroy() clears all onPendingRequestsChanged listeners', async () => {
    const pendingCb = vi.fn();
    service.onPendingRequestsChanged(pendingCb);
    await service.connectAdapter(adapter);
    service.destroy();
    await service.connectAdapter(adapter);
    adapter.listeners.get('friend-request-received')!({ payload: null });
    expect(pendingCb).not.toHaveBeenCalled();
  });

  // ── ZEB-376 Phase 2b: peer intro policy + request introduction ───────────

  it('getPeerIntroPolicy invokes get_peer_intro_policy and returns the policy', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue('ask');
    const result = await service.getPeerIntroPolicy();
    expect(adapter.invoke).toHaveBeenCalledWith('get_peer_intro_policy', {});
    expect(result).toBe('ask');
  });

  it('setPeerIntroPolicy invokes set_peer_intro_policy with the policy arg', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.setPeerIntroPolicy('ask');
    expect(adapter.invoke).toHaveBeenCalledWith('set_peer_intro_policy', { policy: 'ask' });
  });

  it('requestIntroduction invokes request_introduction with camelCase via/target args', async () => {
    await service.connectAdapter(adapter);
    (adapter.invoke as any).mockResolvedValue(undefined);
    await service.requestIntroduction('ffee', 'aabb');
    expect(adapter.invoke).toHaveBeenCalledWith('request_introduction', {
      viaOwnerIdHex: 'ffee',
      targetOwnerIdHex: 'aabb',
    });
  });
});

describe('contactsFromFriends', () => {
  const base = { establishedVia: 'mutual_key' as const, referrable: false };

  it('maps active friends into Profile entries keyed by ownerIdHex', () => {
    const friends: FriendDto[] = [
      { ownerIdHex: 'd6ff9d98ca9b20df7838abdbddc7d57f', display: 'ZEBbot', status: 'active', ...base },
      { ownerIdHex: '11223344556677889900aabbccddeeff', display: 'Bob', status: 'active', ...base },
    ];
    const contacts = contactsFromFriends(friends);
    expect(contacts.size).toBe(2);
    expect(contacts.get('d6ff9d98ca9b20df7838abdbddc7d57f')).toEqual({
      address: 'd6ff9d98ca9b20df7838abdbddc7d57f',
      displayName: 'ZEBbot',
    });
  });

  it('prefers the local nickname over the published display name', () => {
    const friends: FriendDto[] = [
      {
        ownerIdHex: 'd6ff9d98ca9b20df7838abdbddc7d57f',
        display: 'ZEBbot',
        nickname: 'Ildwyn',
        status: 'active',
        ...base,
      },
    ];
    const contacts = contactsFromFriends(friends);
    expect(contacts.get('d6ff9d98ca9b20df7838abdbddc7d57f')?.displayName).toBe('Ildwyn');
  });

  it('falls back to display, then to a short-hex prefix when neither name is set', () => {
    const friends: FriendDto[] = [
      { ownerIdHex: 'aabbccdd00112233aabbccdd00112233', display: 'Carol', nickname: null, status: 'active', ...base },
      { ownerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef', display: null, status: 'active', ...base },
    ];
    const contacts = contactsFromFriends(friends);
    expect(contacts.get('aabbccdd00112233aabbccdd00112233')?.displayName).toBe('Carol');
    expect(contacts.get('deadbeefdeadbeefdeadbeefdeadbeef')?.displayName).toBe('deadbeef…');
  });

  it('treats empty-string names as absent', () => {
    const friends: FriendDto[] = [
      { ownerIdHex: 'deadbeefdeadbeefdeadbeefdeadbeef', display: '', nickname: '', status: 'active', ...base },
    ];
    const contacts = contactsFromFriends(friends);
    expect(contacts.get('deadbeefdeadbeefdeadbeefdeadbeef')?.displayName).toBe('deadbeef…');
  });

  it('excludes pending friends (they cannot receive DMs yet)', () => {
    const friends: FriendDto[] = [
      { ownerIdHex: 'aabbccdd00112233aabbccdd00112233', display: 'NotYet', status: 'pending', ...base },
      { ownerIdHex: '11223344556677889900aabbccddeeff', display: 'Bob', status: 'active', ...base },
    ];
    const contacts = contactsFromFriends(friends);
    expect(contacts.size).toBe(1);
    expect(contacts.has('aabbccdd00112233aabbccdd00112233')).toBe(false);
  });

  it('returns an empty map for an empty friend list', () => {
    expect(contactsFromFriends([]).size).toBe(0);
  });
});
