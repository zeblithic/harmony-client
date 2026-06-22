import { describe, it, expect, vi, beforeEach } from 'vitest';
import { PresenceService } from '../presence-service';
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
  return { ownerIdHex, online, lastSeenMs: 1000, deviceCount: online ? 1 : 0 };
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
});
