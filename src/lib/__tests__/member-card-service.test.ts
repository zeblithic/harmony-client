import { describe, it, expect, vi } from 'vitest';
import { MemberCardService } from '../member-card-service';
import type { TauriAdapter } from '../zenoh-service';

describe('MemberCardService self-seed', () => {
  it('resolves the self owner_id to the local profile name/status synchronously', () => {
    const svc = new MemberCardService();
    svc.seedSelf('685e4ba76a8fde38ecbd2ff5c138df8c', { displayName: 'Jake (Koya Dev)', statusText: 'building' });
    expect(svc.resolve('685e4ba76a8fde38ecbd2ff5c138df8c')).toEqual({ displayName: 'Jake (Koya Dev)', statusText: 'building' });
  });
  it('returns undefined for an unknown owner_id (caller falls back to hash prefix)', () => {
    const svc = new MemberCardService();
    expect(svc.resolve('deadbeefdeadbeefdeadbeefdeadbeef')).toBeUndefined();
  });
  it('seedSelf overwrites the same owner_id on re-seed (profile edited)', () => {
    const svc = new MemberCardService();
    svc.seedSelf('aa'.repeat(16), { displayName: 'old', statusText: '' });
    svc.seedSelf('aa'.repeat(16), { displayName: 'new', statusText: 'hi' });
    expect(svc.resolve('aa'.repeat(16))).toEqual({ displayName: 'new', statusText: 'hi' });
  });
});

describe('MemberCardService cross-peer resolution', () => {
  const ownerA = 'aa'.repeat(16); // 32-char hex
  const selfKey = 'bb'.repeat(16);

  /** Builds a fake adapter that mirrors the 3 card IPCs. */
  function makeAdapter() {
    let nextId = 1;
    const subscribed: string[] = [];
    const unsubscribed: number[] = [];
    const invoke = vi.fn(async (cmd: string, args?: unknown) => {
      const a = (args ?? {}) as Record<string, unknown>;
      switch (cmd) {
        case 'subscribe_member_card':
          subscribed.push(a.ownerIdHex as string);
          return nextId++;
        case 'get_cached_member_card':
          // Return a fixed card for the target owner's subscription (id 1),
          // null for any other.
          if (a.subscriptionId === 1) {
            return { ownerIdHex: ownerA, displayName: 'Alice', statusText: 'hi' };
          }
          return null;
        case 'unsubscribe_member_card':
          unsubscribed.push(a.subscriptionId as number);
          return undefined;
        default:
          throw new Error(`unexpected IPC ${cmd}`);
      }
    });
    const adapter = {
      invoke,
      listen: vi.fn(async () => () => {}),
    } as unknown as TauriAdapter;
    return { adapter, invoke, subscribed, unsubscribed };
  }

  it('subscribes to a visible peer and resolves its card after a poll', async () => {
    const { adapter, subscribed } = makeAdapter();
    const svc = new MemberCardService(adapter);
    await svc.subscribeVisible([ownerA]);
    expect(subscribed).toEqual([ownerA]);
    expect(svc.resolve(ownerA)).toBeUndefined(); // not yet drained
    await svc.pollOnce();
    expect(svc.resolve(ownerA)).toEqual({ displayName: 'Alice', statusText: 'hi' });
  });

  it('fires onUpdate when a poll caches a new card', async () => {
    const { adapter } = makeAdapter();
    const svc = new MemberCardService(adapter);
    const onUpdate = vi.fn();
    svc.onUpdate = onUpdate;
    await svc.subscribeVisible([ownerA]);
    onUpdate.mockClear();
    await svc.pollOnce();
    expect(onUpdate).toHaveBeenCalledTimes(1);
    // Second poll with no change must NOT re-fire.
    await svc.pollOnce();
    expect(onUpdate).toHaveBeenCalledTimes(1);
  });

  it('does NOT subscribe to the self owner_id even when in the visible list', async () => {
    const { adapter, subscribed } = makeAdapter();
    const svc = new MemberCardService(adapter);
    svc.seedSelf(selfKey, { displayName: 'Me', statusText: '' });
    await svc.subscribeVisible([selfKey, ownerA]);
    expect(subscribed).toEqual([ownerA]);
    expect(subscribed).not.toContain(selfKey);
  });

  it('keeps self authoritative — poll never overwrites the self card', async () => {
    const { adapter } = makeAdapter();
    const svc = new MemberCardService(adapter);
    svc.seedSelf(ownerA, { displayName: 'MySelf', statusText: 'local' });
    // ownerA is self here; subscribeVisible filters it, so no network card lands.
    await svc.subscribeVisible([ownerA]);
    await svc.pollOnce();
    expect(svc.resolve(ownerA)).toEqual({ displayName: 'MySelf', statusText: 'local' });
  });

  it('unsubscribes departed owners on a narrowed visible set', async () => {
    const { adapter, unsubscribed } = makeAdapter();
    const svc = new MemberCardService(adapter);
    await svc.subscribeVisible([ownerA]); // id 1
    await svc.subscribeVisible([]); // ownerA departed
    expect(unsubscribed).toEqual([1]);
  });

  it('unsubscribeAll cancels the poll loop and unsubscribes every active sub', async () => {
    const { adapter, unsubscribed } = makeAdapter();
    const svc = new MemberCardService(adapter);
    await svc.subscribeVisible([ownerA, 'cc'.repeat(16)]); // ids 1, 2
    await svc.unsubscribeAll();
    expect(unsubscribed.sort()).toEqual([1, 2]);
  });

  it('drives the poll loop on a real timer tick (fake timers)', async () => {
    vi.useFakeTimers();
    try {
      const { adapter } = makeAdapter();
      const svc = new MemberCardService(adapter);
      await svc.subscribeVisible([ownerA]);
      // Advance past POLL_INTERVAL_MS (3000ms) and flush the async tick.
      await vi.advanceTimersByTimeAsync(3000);
      expect(svc.resolve(ownerA)).toEqual({ displayName: 'Alice', statusText: 'hi' });
      await svc.unsubscribeAll();
    } finally {
      vi.useRealTimers();
    }
  });

  it('network methods no-op without an adapter (non-connected boot)', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    try {
      const svc = new MemberCardService(); // no adapter
      await expect(svc.subscribeVisible([ownerA])).resolves.toBeUndefined();
      await expect(svc.pollOnce()).resolves.toBeUndefined();
      expect(svc.resolve(ownerA)).toBeUndefined();
    } finally {
      warn.mockRestore();
    }
  });
});
