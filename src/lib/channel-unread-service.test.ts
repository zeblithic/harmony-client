import { describe, it, expect, vi } from 'vitest';
import {
  ChannelUnreadService,
  UNREAD_TRACK_CAP,
  type ChannelUnreadDeps,
} from './channel-unread-service';
import type { ChannelMessageDto } from './channel-message-service';
import type { ChannelInfo } from './community-service';
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';

const hlc = (wallMs: number, logical = 0, deviceId = 'peer'): Hlc => ({
  wallMs,
  logical,
  deviceId,
});
const msg = (id: string, at: Hlc, author = 'peer-1'): ChannelMessageDto =>
  ({ messageId: id, communityId: 'c1', channelId: 'ch1', author, at, body: [] }) as ChannelMessageDto;
const ch = (id: string, name = id): ChannelInfo =>
  ({ channelId: id, name, writePower: 0, kind: 'text', createdAt: hlc(0) }) as ChannelInfo;

class MemStore implements UnreadCursorStore {
  owner: string | null = null;
  map = new Map<string, Hlc>();
  connectOwner(o: string) {
    this.owner = o;
  }
  get(c: string, chId: string) {
    return this.owner ? (this.map.get(`${c}:${chId}`) ?? null) : null;
  }
  set(c: string, chId: string, h: Hlc) {
    if (this.owner) this.map.set(`${c}:${chId}`, h);
  }
}

function harness(over: Partial<ChannelUnreadDeps> = {}) {
  const store = new MemStore();
  store.connectOwner('me');
  const pushes: Array<[string, number]> = [];
  const deps: ChannelUnreadDeps = {
    listMessagesSince: vi.fn(async () => []),
    setUnread: (chId, n) => pushes.push([chId, n]),
    isActiveChannel: () => false,
    isFocused: () => true,
    selfOwnerId: () => 'me',
    storage: store,
    now: () => 5000,
    ...over,
  };
  return { svc: new ChannelUnreadService(deps), deps, store, pushes };
}
const lastCount = (pushes: Array<[string, number]>, chId: string) =>
  [...pushes].reverse().find(([id]) => id === chId)?.[1];

describe('ChannelUnreadService (ZEB-665)', () => {
  it('start-clean: no stored cursor → stamps now() and pushes 0, no IPC', async () => {
    const { svc, deps, store, pushes } = harness();
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(store.get('c1', 'ch1')).toEqual({ wallMs: 5000, logical: 0, deviceId: '' });
    expect(deps.listMessagesSince).not.toHaveBeenCalled();
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('seed with stored cursor counts strictly-newer non-self messages', async () => {
    const { svc, store, pushes } = harness({
      listMessagesSince: async () => [
        msg('m1', hlc(200)),
        msg('m2', hlc(300)),
        msg('mine', hlc(400), 'me'),
      ],
    });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(lastCount(pushes, 'ch1')).toBe(2); // self-authored dropped
  });

  it('seed overflow caps at UNREAD_TRACK_CAP', async () => {
    const many = Array.from({ length: UNREAD_TRACK_CAP }, (_, i) => msg(`m${i}`, hlc(200 + i)));
    const { svc, store, pushes } = harness({ listMessagesSince: async () => many });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(lastCount(pushes, 'ch1')).toBe(UNREAD_TRACK_CAP);
  });

  it('live message for a non-active channel counts once (backfill re-emission dedupes)', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // re-emitted by backfill
    expect(lastCount(pushes, 'ch1')).toBe(1);
  });

  it('messages at or before the cursor never count (history replay)', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('old', hlc(50)));
    svc.onMessage('c1', 'ch1', msg('at-cursor', hlc(100)));
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('self-authored messages never count', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('mine', hlc(200), 'me'));
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('focused + active channel advances the cursor instead of counting', async () => {
    const { svc, store, pushes } = harness({ isActiveChannel: () => true, isFocused: () => true });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(0);
    expect(store.get('c1', 'ch1')).toEqual(hlc(200));
  });

  it('unfocused + active channel still counts (mirrors mention semantics)', async () => {
    const { svc, store, pushes } = harness({ isActiveChannel: () => true, isFocused: () => false });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(1);
  });

  it('a focused arrival does NOT wipe the unfocused backlog (spec §6)', async () => {
    let focused = false;
    const { svc, store, pushes } = harness({
      isActiveChannel: () => true,
      isFocused: () => focused,
    });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // unfocused → counts
    expect(lastCount(pushes, 'ch1')).toBe(1);
    focused = true;
    svc.onMessage('c1', 'ch1', msg('m2', hlc(300))); // focused → read on landing
    expect(lastCount(pushes, 'ch1')).toBe(1); // backlog badge survives
    expect(store.get('c1', 'ch1')).toEqual(hlc(300)); // cursor still advances
    svc.markChannelRead('c1', 'ch1'); // only an explicit open clears
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('a focused re-emission of a previously-counted message uncounts just that one', async () => {
    let focused = false;
    const { svc, store, pushes } = harness({
      isActiveChannel: () => true,
      isFocused: () => focused,
    });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    svc.onMessage('c1', 'ch1', msg('m2', hlc(250)));
    expect(lastCount(pushes, 'ch1')).toBe(2);
    focused = true;
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // re-emitted while viewing
    expect(lastCount(pushes, 'ch1')).toBe(1); // m1 uncounted, m2 remains
  });

  it('markChannelRead wipes the set, pushes 0, and stamps past maxSeen', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(9000))); // beyond the now() stamp
    svc.markChannelRead('c1', 'ch1');
    expect(lastCount(pushes, 'ch1')).toBe(0);
    expect(store.get('c1', 'ch1')).toEqual(hlc(9000)); // maxSeen wins over now()=5000
    svc.onMessage('c1', 'ch1', msg('m1', hlc(9000))); // replayed after read
    expect(lastCount(pushes, 'ch1')).toBe(0);
  });

  it('markChannelRead under seed-overflow stamps at least now() (open-clears-all)', async () => {
    const many = Array.from({ length: UNREAD_TRACK_CAP }, (_, i) => msg(`m${i}`, hlc(200 + i)));
    const { svc, store } = harness({ listMessagesSince: async () => many });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.markChannelRead('c1', 'ch1');
    const cur = store.get('c1', 'ch1')!;
    expect(cur.wallMs).toBeGreaterThanOrEqual(5000); // ≥ now(), not the oldest-100 tail
  });

  it('event racing the seed unions into one count (no double-count)', async () => {
    let resolveList!: (v: ChannelMessageDto[]) => void;
    const { svc, store, pushes } = harness({
      listMessagesSince: () =>
        new Promise((r) => {
          resolveList = r;
        }),
    });
    store.set('c1', 'ch1', hlc(100));
    const seeding = svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200))); // arrives mid-seed
    resolveList([msg('m1', hlc(200)), msg('m2', hlc(300))]);
    await seeding;
    expect(lastCount(pushes, 'ch1')).toBe(2); // m1 counted once
  });

  it('unseeded channel ignores events (start-clean will cover it)', () => {
    const { svc, pushes } = harness();
    svc.onMessage('c1', 'ch-unknown', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch-unknown')).toBeUndefined();
  });

  it('seed failure warns, stays at 0, and is retried on next materialize', async () => {
    const listMessagesSince = vi
      .fn()
      .mockRejectedValueOnce(new Error('boom'))
      .mockResolvedValueOnce([msg('m1', hlc(200))]);
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const { svc, store, pushes } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(warn).toHaveBeenCalled();
    await svc.onChannelsMaterialized('c1', [ch('ch1')]); // retry succeeds
    expect(lastCount(pushes, 'ch1')).toBe(1);
    warn.mockRestore();
  });

  it('re-materialize does not re-seed but re-pushes known counts', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store, pushes } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    pushes.length = 0;
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(1); // no second IPC
    expect(lastCount(pushes, 'ch1')).toBe(1); // but count re-pushed
  });

  it('connectOwner re-seeds remembered communities under the new owner', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(1);
    svc.connectOwner('me');
    await vi.waitFor(() => expect(listMessagesSince).toHaveBeenCalledTimes(2));
  });

  it('onCommunityRemoved drops session state so a later re-add re-seeds', async () => {
    const listMessagesSince = vi.fn(async () => [msg('m1', hlc(200))]);
    const { svc, store } = harness({ listMessagesSince });
    store.set('c1', 'ch1', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    svc.onCommunityRemoved('c1');
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    expect(listMessagesSince).toHaveBeenCalledTimes(2);
  });

  it('per-community isolation: counts on c1 do not leak to c2 channels', async () => {
    const { svc, store, pushes } = harness();
    store.set('c1', 'ch1', hlc(100));
    store.set('c2', 'chX', hlc(100));
    await svc.onChannelsMaterialized('c1', [ch('ch1')]);
    await svc.onChannelsMaterialized('c2', [ch('chX')]);
    svc.onMessage('c1', 'ch1', msg('m1', hlc(200)));
    expect(lastCount(pushes, 'ch1')).toBe(1);
    expect(lastCount(pushes, 'chX')).toBe(0);
  });
});
