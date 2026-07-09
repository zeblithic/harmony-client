import { describe, it, expect, vi } from 'vitest';
import { DmUnreadService, type DmUnreadDeps, type DmThreadPageEntry } from './dm-unread-service';
import { UNREAD_TRACK_CAP } from './channel-unread-service';
import type { Hlc } from './types';
import type { UnreadCursorStore } from './unread-cursor-store';

const entry = (
  messageCid: string,
  receivedAt: number,
  isSelfOutbound = false,
): DmThreadPageEntry => ({
  messageCid,
  from: isSelfOutbound ? 'me' : 'peer',
  receivedAt,
  isSelfOutbound,
});
const arrival = (messageCid: string, receivedAt: number, from = 'peer') => ({
  spaceId: 's1',
  messageCid,
  from,
  receivedAt,
});

class MemStore implements UnreadCursorStore {
  owner: string | null = null;
  map = new Map<string, Hlc>();
  connectOwner(o: string) {
    this.owner = o;
  }
  get(ns: string, id: string) {
    return this.owner ? (this.map.get(`${ns}:${id}`) ?? null) : null;
  }
  set(ns: string, id: string, h: Hlc) {
    if (this.owner) this.map.set(`${ns}:${id}`, h);
  }
}

function harness(over: Partial<DmUnreadDeps> = {}) {
  const store = new MemStore();
  store.connectOwner('me');
  const pushes: Array<[string, number]> = [];
  const deps: DmUnreadDeps = {
    listThreadPage: vi.fn(async () => []),
    setUnread: (id, n) => pushes.push([id, n]),
    isActiveThread: () => false,
    isFocused: () => true,
    selfOwnerId: () => 'me',
    storage: store,
    now: () => 5000,
    ...over,
  };
  return { svc: new DmUnreadService(deps), deps, store, pushes };
}
const lastCount = (pushes: Array<[string, number]>, id: string) =>
  [...pushes].reverse().find(([sid]) => sid === id)?.[1];
const cursorMs = (store: MemStore, id: string) => store.get('dm', id)?.wallMs;

describe('DmUnreadService (ZEB-666)', () => {
  it('start-clean: no stored cursor → stamps now() and pushes 0, no IPC', async () => {
    const { svc, deps, store, pushes } = harness();
    await svc.onDmSpaceMaterialized('s1');
    expect(store.get('dm', 's1')).toEqual({ wallMs: 5000, logical: 0, deviceId: '' });
    expect(deps.listThreadPage).not.toHaveBeenCalled();
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('seed with stored cursor counts strictly-newer non-self entries', async () => {
    const { svc, store, pushes } = harness({
      listThreadPage: async () => [
        entry('m4', 400, true), // self-outbound → dropped
        entry('m3', 300),
        entry('m2', 200),
        entry('m1', 100), // == cursor → dropped (strict >)
        entry('m0', 50), // older → dropped
      ],
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(2);
  });

  it('seed overflow caps at UNREAD_TRACK_CAP', async () => {
    const many = Array.from(
      { length: UNREAD_TRACK_CAP + 20 },
      (_, i) => entry(`m${i}`, 5000 - i), // newest-first, all > cursor
    );
    const { svc, store, pushes } = harness({ listThreadPage: async () => many });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(UNREAD_TRACK_CAP);
  });

  it('seed failure un-marks seeded (retried on next materialize) and still pushes', async () => {
    let calls = 0;
    const { svc, pushes, store } = harness({
      listThreadPage: async () => {
        calls++;
        if (calls === 1) throw new Error('ipc down');
        return [entry('m1', 200)];
      },
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    expect(lastCount(pushes, 's1')).toBe(0); // failed seed → empty set, still pushed
    await svc.onDmSpaceMaterialized('s1'); // retry succeeds
    expect(lastCount(pushes, 's1')).toBe(1);
  });

  it('seed in flight when markThreadRead lands filters against the fresh cursor (TOCTOU)', async () => {
    let resolvePage!: (v: DmThreadPageEntry[]) => void;
    const { svc, store, pushes } = harness({
      listThreadPage: () => new Promise<DmThreadPageEntry[]>((r) => { resolvePage = r; }),
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    const seeding = svc.onDmSpaceMaterialized('s1'); // seed awaits the page
    svc.markThreadRead('s1'); // user opens the thread mid-flight → cursor stamps now()
    resolvePage([entry('m2', 300), entry('m1', 200)]); // newer than the OLD cursor only
    await seeding;
    expect(lastCount(pushes, 's1')).toBe(0); // nothing resurrected past the read stamp
  });

  it('live arrival for a non-active thread counts once (re-delivery dedupes)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 200));
    svc.onDmReceived(arrival('m1', 200));
    expect(lastCount(pushes, 's1')).toBe(1);
  });

  it('arrivals at or before the cursor never count (strict >)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('old', 50));
    svc.onDmReceived(arrival('at-cursor', 100));
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('self arrivals (from === selfOwnerId) never count', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('mine', 200, 'me'));
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('no-cursor arrival is ignored (start-clean at materialize covers it)', () => {
    const { svc, pushes } = harness();
    svc.onDmReceived(arrival('m1', 200));
    expect(pushes.length).toBe(0);
  });

  it('focused+active arrival advances the cursor instead of counting', async () => {
    const { svc, store, pushes } = harness({
      isActiveThread: (id) => id === 's1',
      isFocused: () => true,
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('live', 150));
    expect(lastCount(pushes, 's1')).toBe(0);
    expect(cursorMs(store, 's1')).toBe(150);
  });

  it('focused+active re-delivery of a counted CID removes it, preserving the rest', async () => {
    let focused = false;
    const { svc, store, pushes } = harness({
      isActiveThread: (id) => id === 's1',
      isFocused: () => focused,
    });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('b1', 150));
    svc.onDmReceived(arrival('b2', 160));
    expect(lastCount(pushes, 's1')).toBe(2);
    focused = true; // user focuses the window with the thread open
    svc.onDmReceived(arrival('b2', 160)); // re-delivery of a counted message
    expect(lastCount(pushes, 's1')).toBe(1); // b2 uncounted, b1 backlog preserved
    expect(cursorMs(store, 's1')).toBe(160);
  });

  it('markThreadRead stamps max(cursor, maxSeen, now) and clears the set', async () => {
    const { svc, store, pushes } = harness({ now: () => 5000 });
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 9000)); // receivedAt ahead of now()
    expect(lastCount(pushes, 's1')).toBe(1);
    svc.markThreadRead('s1');
    expect(lastCount(pushes, 's1')).toBe(0);
    expect(cursorMs(store, 's1')).toBe(9000); // maxSeen wins over now()
    svc.onDmReceived(arrival('m1', 9000)); // replay of the read message
    expect(lastCount(pushes, 's1')).toBe(0);
  });

  it('connectOwner wipes session state and replays materialized spaces', async () => {
    const { svc, store, pushes } = harness();
    await svc.onDmSpaceMaterialized('s1'); // start-clean under 'me'
    svc.onDmReceived(arrival('m1', 6000));
    expect(lastCount(pushes, 's1')).toBe(1);
    store.connectOwner('other'); // MemStore keeps map; real store reloads per owner
    svc.connectOwner('other');
    await new Promise((r) => setTimeout(r, 0)); // drain the replayed async seed
    expect(lastCount(pushes, 's1')).toBe(0); // fresh session state for the new owner
  });

  it('onDmSpaceRemoved drops session state (cursor kept)', async () => {
    const { svc, store, pushes } = harness();
    store.set('dm', 's1', { wallMs: 100, logical: 0, deviceId: '' });
    await svc.onDmSpaceMaterialized('s1');
    svc.onDmReceived(arrival('m1', 200));
    expect(lastCount(pushes, 's1')).toBe(1);
    svc.onDmSpaceRemoved('s1');
    expect(store.get('dm', 's1')).not.toBeNull(); // cursor survives removal
    // After removal the space is unseeded; a later arrival with a cursor
    // still counts (channel parity: gate is the cursor, not seededness).
    svc.onDmReceived(arrival('m2', 300));
    expect(lastCount(pushes, 's1')).toBe(1);
  });
});
