import { describe, it, expect, vi, beforeEach } from 'vitest';
import { PresenceService, PRESENCE_STALE_AFTER_MS } from '../presence-service';
import type { PresenceMemberDto } from '../presence-service';
import type { TauriAdapter } from '../zenoh-service';

function makeAdapter(): TauriAdapter & { listeners: Map<string, Function>; unlistens: Map<string, ReturnType<typeof vi.fn>> } {
  const listeners = new Map<string, Function>();
  const unlistens = new Map<string, ReturnType<typeof vi.fn>>();
  return {
    listeners,
    unlistens,
    invoke: vi.fn(),
    listen: vi.fn(async (event: string, handler: Function) => {
      listeners.set(event, handler);
      const unlisten = vi.fn(() => listeners.delete(event));
      unlistens.set(event, unlisten);
      return unlisten;
    }),
  } as any;
}

const CID_A = 'aa'.repeat(16);
const CID_B = 'bb'.repeat(16);
const OWNER_1 = '11'.repeat(16);
const OWNER_2 = '22'.repeat(16);

function member(ownerIdHex: string, online: boolean): PresenceMemberDto {
  // ZEB-972: a fresh beacon stamp — the service now freshness-gates `online`,
  // so a fixture with an ancient lastSeenMs would read stale, not online.
  return { ownerIdHex, online, lastSeenMs: Date.now(), deviceCount: online ? 1 : 0 };
}

describe('PresenceService', () => {
  let service: PresenceService;
  let adapter: ReturnType<typeof makeAdapter>;

  beforeEach(() => {
    service = new PresenceService(adapter = makeAdapter());
  });

  it('subscribe invokes subscribe_community_presence with camelCase args and seeds via get_community_presence', async () => {
    const seed = [member(OWNER_1, true), member(OWNER_2, false)];
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return seed;
      return undefined;
    });
    const onUpdate = vi.fn();

    await service.subscribe(CID_A, onUpdate);

    expect(adapter.invoke).toHaveBeenCalledWith('subscribe_community_presence', { communityId: CID_A });
    expect(adapter.invoke).toHaveBeenCalledWith('get_community_presence', { communityId: CID_A });
    // initial state seeded from get_community_presence
    expect(onUpdate).toHaveBeenCalledWith(seed);
    expect(service.isOnline(OWNER_1)).toBe(true);
    expect(service.isOnline(OWNER_2)).toBe(false);
  });

  it('presence-updated event for the subscribed community drives onUpdate + isOnline', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    const onUpdate = vi.fn();
    await service.subscribe(CID_A, onUpdate);
    onUpdate.mockClear();

    const members = [member(OWNER_1, true)];
    const handler = adapter.listeners.get('presence-updated')!;
    handler({ payload: { communityId: CID_A, members } });

    expect(onUpdate).toHaveBeenCalledWith(members);
    expect(service.isOnline(OWNER_1)).toBe(true);
  });

  it('presence-updated event for a DIFFERENT community is ignored', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    const onUpdate = vi.fn();
    await service.subscribe(CID_A, onUpdate);
    onUpdate.mockClear();

    const handler = adapter.listeners.get('presence-updated')!;
    handler({ payload: { communityId: CID_B, members: [member(OWNER_1, true)] } });

    expect(onUpdate).not.toHaveBeenCalled();
    expect(service.isOnline(OWNER_1)).toBe(false);
  });

  it('unsubscribe invokes unsubscribe_community_presence with camelCase args and removes the listener', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    const unlisten = adapter.unlistens.get('presence-updated')!;

    await service.unsubscribe(CID_A);

    expect(adapter.invoke).toHaveBeenCalledWith('unsubscribe_community_presence', { communityId: CID_A });
    expect(unlisten).toHaveBeenCalled();
    expect(adapter.listeners.has('presence-updated')).toBe(false);
  });

  it('after unsubscribe a presence-updated event no longer drives onUpdate', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    const onUpdate = vi.fn();
    await service.subscribe(CID_A, onUpdate);
    const handler = adapter.listeners.get('presence-updated')!;
    await service.unsubscribe(CID_A);
    onUpdate.mockClear();

    // The listener was removed; invoking the stale handler must not call back.
    handler({ payload: { communityId: CID_A, members: [member(OWNER_1, true)] } });
    expect(onUpdate).not.toHaveBeenCalled();
  });

  it('getPresence invokes get_community_presence with camelCase args and returns the DTOs', async () => {
    const dtos = [member(OWNER_1, true)];
    (adapter.invoke as any).mockResolvedValue(dtos);
    const out = await service.getPresence(CID_A);
    expect(adapter.invoke).toHaveBeenCalledWith('get_community_presence', { communityId: CID_A });
    expect(out).toEqual(dtos);
  });

  it('isOnline is case-insensitive on ownerIdHex', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [member(OWNER_1.toUpperCase(), true)];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    expect(service.isOnline(OWNER_1)).toBe(true);
    expect(service.isOnline(OWNER_1.toUpperCase())).toBe(true);
  });

  it('isOnline is scoped to the ACTIVE community: a non-active community does not leak online', async () => {
    // Subscribe to CID_A (OWNER_1 online there), then subscribe to CID_B
    // (no members). CID_B is now the active community, so OWNER_1 — online only
    // in the lingering CID_A map — must NOT read as online.
    (adapter.invoke as any).mockImplementation(async (cmd: string, args: { communityId: string }) => {
      if (cmd === 'get_community_presence') {
        return args.communityId === CID_A ? [member(OWNER_1, true)] : [];
      }
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    expect(service.isOnline(OWNER_1)).toBe(true); // CID_A is active
    await service.subscribe(CID_B, vi.fn());
    // CID_B is now active and has no members → OWNER_1 must not leak from CID_A.
    expect(service.isOnline(OWNER_1)).toBe(false);
  });

  it('subscribe with { setActive: false } does not repoint the active community (boot subscribe-all)', async () => {
    // ZEB-600 / CodeRabbit #381: boot subscribe-all subscribes every community
    // but must NOT clobber the selected active roster. CID_A is selected/active;
    // a background subscribe of CID_B with setActive:false leaves CID_A active.
    (adapter.invoke as any).mockImplementation(async (cmd: string, args: { communityId: string }) => {
      if (cmd === 'get_community_presence') {
        return args.communityId === CID_A ? [member(OWNER_1, true)] : [];
      }
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn()); // selection path → CID_A active
    expect(service.isOnline(OWNER_1)).toBe(true);
    await service.subscribe(CID_B, vi.fn(), { setActive: false }); // background
    expect(service.isSubscribed(CID_B)).toBe(true); // CID_B is live...
    expect(service.isOnline(OWNER_1)).toBe(true); // ...but CID_A stays active
  });

  it('seed (get_community_presence) rejection rolls back the partial subscription and rethrows', async () => {
    const unlistenSpy = adapter.unlistens;
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'subscribe_community_presence') return undefined;
      if (cmd === 'get_community_presence') throw 'seed boom';
      if (cmd === 'unsubscribe_community_presence') return undefined;
      return undefined;
    });

    await expect(service.subscribe(CID_A, vi.fn())).rejects.toThrow('seed boom');

    // Listener was installed then rolled back via unlisten.
    const unlisten = unlistenSpy.get('presence-updated')!;
    expect(unlisten).toHaveBeenCalled();
    expect(adapter.listeners.has('presence-updated')).toBe(false);
    // Best-effort backend unsubscribe was issued.
    expect(adapter.invoke).toHaveBeenCalledWith('unsubscribe_community_presence', { communityId: CID_A });
    // No active community / cached state left behind.
    expect(service.isOnline(OWNER_1)).toBe(false);
  });

  it('listen(presence-updated) rejection rolls back the backend subscription and rethrows', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'subscribe_community_presence') return undefined;
      if (cmd === 'get_community_presence') return [member(OWNER_1, true)];
      if (cmd === 'unsubscribe_community_presence') return undefined;
      return undefined;
    });
    // The backend subscribe succeeds, but installing the push listener fails.
    (adapter.listen as any).mockRejectedValue('listen boom');

    await expect(service.subscribe(CID_A, vi.fn())).rejects.toThrow('listen boom');

    // No listener was installed (listen rejected), so nothing to unlisten.
    expect(adapter.listeners.has('presence-updated')).toBe(false);
    // Best-effort backend unsubscribe was issued to undo the orphaned subscribe.
    expect(adapter.invoke).toHaveBeenCalledWith('unsubscribe_community_presence', { communityId: CID_A });
    // No active community / cached state left behind.
    expect(service.isOnline(OWNER_1)).toBe(false);
  });

  it('seed rollback rethrows even when the best-effort backend unsubscribe also fails', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'subscribe_community_presence') return undefined;
      if (cmd === 'get_community_presence') throw 'seed boom';
      if (cmd === 'unsubscribe_community_presence') throw 'unsub boom';
      return undefined;
    });
    // The seed error is what surfaces; the swallowed unsubscribe failure must not mask it.
    await expect(service.subscribe(CID_A, vi.fn())).rejects.toThrow('seed boom');
    expect(adapter.listeners.has('presence-updated')).toBe(false);
  });

  it('normalizes a raw-string IPC rejection into an Error (subscribe)', async () => {
    (adapter.invoke as any).mockRejectedValue('no engine for community');
    await expect(service.subscribe(CID_A, vi.fn())).rejects.toThrow('no engine for community');
  });

  it('normalizes a raw-string IPC rejection into an Error (getPresence)', async () => {
    (adapter.invoke as any).mockRejectedValue('no engine for community');
    await expect(service.getPresence(CID_A)).rejects.toThrow('no engine for community');
  });

  it('normalizes a raw-string IPC rejection into an Error (unsubscribe)', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    (adapter.invoke as any).mockRejectedValue('no engine for community');
    await expect(service.unsubscribe(CID_A)).rejects.toThrow('no engine for community');
  });

  it('no-adapter instance no-ops without throwing (subscribe/getPresence/unsubscribe/isOnline)', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const noAdapter = new PresenceService();
    const onUpdate = vi.fn();

    await expect(noAdapter.subscribe(CID_A, onUpdate)).resolves.toBeUndefined();
    await expect(noAdapter.getPresence(CID_A)).resolves.toEqual([]);
    await expect(noAdapter.unsubscribe(CID_A)).resolves.toBeUndefined();
    // subscribe was a no-op, so nothing seeded → onUpdate never fired, isOnline false.
    expect(onUpdate).not.toHaveBeenCalled();
    expect(noAdapter.isOnline(OWNER_1)).toBe(false);

    warn.mockRestore();
  });

  it('onlineCount counts online members in a community (0 for unknown)', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [member(OWNER_1, true), member(OWNER_2, false)];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    expect(service.onlineCount(CID_A)).toBe(1);
    expect(service.onlineCount(CID_B)).toBe(0); // unsubscribed / unknown
  });

  it('hasOthersOnline excludes self (case-insensitive)', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [member(OWNER_1, true)];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
    // Only OWNER_1 online → from OWNER_1's own view, nobody else is around.
    expect(service.hasOthersOnline(CID_A, OWNER_1)).toBe(false);
    expect(service.hasOthersOnline(CID_A, OWNER_1.toUpperCase())).toBe(false);
    // From OWNER_2's view, OWNER_1 is someone else who's online.
    expect(service.hasOthersOnline(CID_A, OWNER_2)).toBe(true);
    expect(service.hasOthersOnline(CID_B, OWNER_2)).toBe(false); // unknown community
  });

  it('isOnlineAnywhere is true if online in ANY subscribed community (case-insensitive)', async () => {
    (adapter.invoke as any).mockImplementation(async (cmd: string, args: { communityId: string }) => {
      if (cmd === 'get_community_presence') {
        return args.communityId === CID_A ? [member(OWNER_1, true)] : [];
      }
      return undefined;
    });
    // Subscribe-all keeps BOTH community maps (unlike isOnline, which is
    // active-only). CID_B becomes active but CID_A's roster is retained.
    await service.subscribe(CID_A, vi.fn());
    await service.subscribe(CID_B, vi.fn());
    expect(service.isOnlineAnywhere(OWNER_1)).toBe(true);
    expect(service.isOnlineAnywhere(OWNER_1.toUpperCase())).toBe(true);
    expect(service.isOnlineAnywhere(OWNER_2)).toBe(false);
  });
});

// ZEB-972 — client-side staleness honesty. The backend's 30 s TTL sweeper is
// the SOLE eviction path for presence rows, and it lives in the event loop: if
// that loop stalls or dies (the ZEB-970 wedge incident), rows freeze in place
// and dots would stay green forever. These tests pin the client-side guard:
// a row whose beacon is older than PRESENCE_STALE_AFTER_MS (2× the backend
// TTL — possible only when backend eviction is overdue) reads `stale`, never
// `online`, and an evicted row keeps its last-known beacon stamp for tooltips.
describe('PresenceService staleness honesty (ZEB-972)', () => {
  let now: number;
  let service: PresenceService;
  let adapter: ReturnType<typeof makeAdapter>;

  function stamped(ownerIdHex: string, lastSeenMs: number): PresenceMemberDto {
    return { ownerIdHex, online: true, lastSeenMs, deviceCount: 1 };
  }

  function push(members: PresenceMemberDto[], communityId = CID_A) {
    adapter.listeners.get('presence-updated')!({ payload: { communityId, members } });
  }

  beforeEach(async () => {
    now = 1_000_000_000;
    adapter = makeAdapter();
    service = new PresenceService(adapter, { now: () => now });
    (adapter.invoke as any).mockImplementation(async (cmd: string) => {
      if (cmd === 'get_community_presence') return [];
      return undefined;
    });
    await service.subscribe(CID_A, vi.fn());
  });

  it('fresh beacon → online, carrying lastSeenMs', () => {
    push([stamped(OWNER_1, now - 5_000)]);
    expect(service.presenceFor(OWNER_1)).toEqual({ state: 'online', lastSeenMs: now - 5_000 });
    expect(service.isOnline(OWNER_1)).toBe(true);
  });

  it('beacon exactly at the threshold is still online (boundary)', () => {
    push([stamped(OWNER_1, now - PRESENCE_STALE_AFTER_MS)]);
    expect(service.presenceFor(OWNER_1).state).toBe('online');
  });

  it('row present but beacon overdue for eviction → stale, and stale is excluded everywhere', () => {
    const seen = now - 5_000;
    push([stamped(OWNER_1, seen)]);
    now += 120_000; // 2 min pass with no event and no eviction — the incident class
    expect(service.presenceFor(OWNER_1)).toEqual({ state: 'stale', lastSeenMs: seen });
    expect(service.isOnline(OWNER_1)).toBe(false);
    expect(service.onlineCount(CID_A)).toBe(0);
    expect(service.hasOthersOnline(CID_A, OWNER_2)).toBe(false);
    expect(service.isOnlineAnywhere(OWNER_1)).toBe(false);
  });

  it('evicted row → offline, retaining the last-known beacon stamp for tooltips', () => {
    const seen = now - 5_000;
    push([stamped(OWNER_1, seen)]);
    push([]); // backend sweep evicted the row
    expect(service.presenceFor(OWNER_1)).toEqual({ state: 'offline', lastSeenMs: seen });
    expect(service.isOnline(OWNER_1)).toBe(false);
  });

  it('never-seen owner → offline with no lastSeenMs', () => {
    expect(service.presenceFor(OWNER_2)).toEqual({ state: 'offline' });
  });

  it('last-known stamp max-merges — an out-of-order older delivery cannot regress it', () => {
    push([stamped(OWNER_1, now - 5_000)]);
    push([stamped(OWNER_1, now - 50_000)]); // older stamp re-delivered
    push([]);
    expect(service.presenceFor(OWNER_1).lastSeenMs).toBe(now - 5_000);
  });
});
