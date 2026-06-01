import { describe, it, expect, vi, afterEach } from 'vitest';
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

  // Track every service these tests create so afterEach can tear down their
  // timer-driven poll loops + subscriptions — otherwise a subscribed service
  // leaks its setInterval into later tests.
  let services: MemberCardService[] = [];
  function makeService(adapter?: TauriAdapter): MemberCardService {
    const svc = new MemberCardService(adapter);
    services.push(svc);
    return svc;
  }

  afterEach(async () => {
    for (const svc of services) {
      await svc.unsubscribeAll();
    }
    services = [];
    vi.useRealTimers();
  });

  /** Builds a fake adapter that mirrors the 3 card IPCs. */
  function makeAdapter() {
    let nextId = 1;
    const subscribed: string[] = [];
    const createdIds: number[] = [];
    const unsubscribed: number[] = [];
    const invoke = vi.fn(async (cmd: string, args?: unknown) => {
      const a = (args ?? {}) as Record<string, unknown>;
      switch (cmd) {
        case 'subscribe_member_card': {
          subscribed.push(a.ownerIdHex as string);
          const id = nextId++;
          createdIds.push(id);
          return id;
        }
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
    return { adapter, invoke, subscribed, createdIds, unsubscribed };
  }

  it('subscribes to a visible peer and resolves its card after a poll', async () => {
    const { adapter, subscribed } = makeAdapter();
    const svc = makeService(adapter);
    await svc.subscribeVisible([ownerA]);
    expect(subscribed).toEqual([ownerA]);
    expect(svc.resolve(ownerA)).toBeUndefined(); // not yet drained
    await svc.pollOnce();
    expect(svc.resolve(ownerA)).toEqual({ displayName: 'Alice', statusText: 'hi' });
  });

  it('threads profilePageRoot from a polled DiscoveredCardInfo into the resolved card', async () => {
    // ZEB-345: the cache-drain path must carry profilePageRoot the same way it
    // carries displayName/statusText.
    const invoke = vi.fn(async (cmd: string, args?: unknown) => {
      const a = (args ?? {}) as Record<string, unknown>;
      if (cmd === 'subscribe_member_card') return 1;
      if (cmd === 'get_cached_member_card' && a.subscriptionId === 1) {
        return {
          ownerIdHex: ownerA,
          displayName: 'Alice',
          statusText: 'hi',
          profilePageRoot: 'cid-polled-page',
        };
      }
      if (cmd === 'unsubscribe_member_card') return undefined;
      // Fail loudly on any unexpected IPC so an accidental call surfaces instead
      // of silently resolving to null (matches makeAdapter's posture).
      throw new Error(`unexpected IPC ${cmd}`);
    });
    const adapter = { invoke, listen: vi.fn(async () => () => {}) } as unknown as TauriAdapter;
    const svc = makeService(adapter);
    await svc.subscribeVisible([ownerA]);
    await svc.pollOnce();
    expect(svc.resolve(ownerA)?.profilePageRoot).toBe('cid-polled-page');
  });

  it('fires onUpdate when a poll caches a new card', async () => {
    const { adapter } = makeAdapter();
    const svc = makeService(adapter);
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
    const svc = makeService(adapter);
    svc.seedSelf(selfKey, { displayName: 'Me', statusText: '' });
    await svc.subscribeVisible([selfKey, ownerA]);
    expect(subscribed).toEqual([ownerA]);
    expect(subscribed).not.toContain(selfKey);
  });

  it('keeps self authoritative — poll never overwrites the self card', async () => {
    const { adapter } = makeAdapter();
    const svc = makeService(adapter);
    svc.seedSelf(ownerA, { displayName: 'MySelf', statusText: 'local' });
    // ownerA is self here; subscribeVisible filters it, so no network card lands.
    await svc.subscribeVisible([ownerA]);
    await svc.pollOnce();
    expect(svc.resolve(ownerA)).toEqual({ displayName: 'MySelf', statusText: 'local' });
  });

  it('unsubscribes departed owners on a narrowed visible set', async () => {
    const { adapter, unsubscribed } = makeAdapter();
    const svc = makeService(adapter);
    await svc.subscribeVisible([ownerA]); // id 1
    await svc.subscribeVisible([]); // ownerA departed
    expect(unsubscribed).toEqual([1]);
  });

  it('stops the poll loop when reconciliation drains all subscriptions to empty', async () => {
    const { adapter } = makeAdapter();
    const svc = makeService(adapter);
    const handle = () =>
      (svc as unknown as { pollHandle: ReturnType<typeof setInterval> | null })
        .pollHandle;
    await svc.subscribeVisible([ownerA]);
    expect(handle()).not.toBeNull(); // loop running while a sub is active
    // Narrow the visible set to empty (the member departed / was filtered) —
    // the diff-reconcile path, NOT unsubscribeAll. The 3s interval must stop
    // rather than keep firing over an empty subs map.
    await svc.subscribeVisible([]);
    expect(handle()).toBeNull();
  });

  it('unsubscribeAll cancels the poll loop and unsubscribes every active sub', async () => {
    const { adapter, unsubscribed } = makeAdapter();
    const svc = makeService(adapter);
    await svc.subscribeVisible([ownerA, 'cc'.repeat(16)]); // ids 1, 2
    await svc.unsubscribeAll();
    expect(unsubscribed.sort()).toEqual([1, 2]);
  });

  it('drives the poll loop on a real timer tick (fake timers)', async () => {
    vi.useFakeTimers();
    try {
      const { adapter } = makeAdapter();
      const svc = makeService(adapter);
      await svc.subscribeVisible([ownerA]);
      // Advance past POLL_INTERVAL_MS (3000ms) and flush the async tick.
      await vi.advanceTimersByTimeAsync(3000);
      expect(svc.resolve(ownerA)).toEqual({ displayName: 'Alice', statusText: 'hi' });
      await svc.unsubscribeAll();
    } finally {
      vi.useRealTimers();
    }
  });

  it('applyCard (push path) resolves a card and fires onUpdate; unchanged re-apply is a no-op', () => {
    const svc = makeService(); // no adapter needed for the push path
    const onUpdate = vi.fn();
    svc.onUpdate = onUpdate;
    svc.applyCard(ownerA, { displayName: 'Alice', statusText: 'hi' });
    expect(svc.resolve(ownerA)).toEqual({ displayName: 'Alice', statusText: 'hi' });
    expect(onUpdate).toHaveBeenCalledTimes(1);
    // Re-applying the identical card must NOT churn / re-fire onUpdate.
    svc.applyCard(ownerA, { displayName: 'Alice', statusText: 'hi' });
    expect(onUpdate).toHaveBeenCalledTimes(1);
  });

  it('applyCard never overwrites the self entry seeded via seedSelf', () => {
    const svc = makeService();
    svc.seedSelf(selfKey, { displayName: 'Me', statusText: 'local' });
    const onUpdate = vi.fn();
    svc.onUpdate = onUpdate;
    svc.applyCard(selfKey, { displayName: 'Spoofed', statusText: 'evil' });
    expect(svc.resolve(selfKey)).toEqual({ displayName: 'Me', statusText: 'local' });
    expect(onUpdate).not.toHaveBeenCalled();
  });

  it('network methods no-op without an adapter (non-connected boot)', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    try {
      const svc = makeService(); // no adapter
      await expect(svc.subscribeVisible([ownerA])).resolves.toBeUndefined();
      await expect(svc.pollOnce()).resolves.toBeUndefined();
      expect(svc.resolve(ownerA)).toBeUndefined();
    } finally {
      warn.mockRestore();
    }
  });

  it('serializes subscribeVisible / unsubscribeAll so a race orphans no backend subscription', async () => {
    const { adapter, createdIds, unsubscribed } = makeAdapter();
    const svc = makeService(adapter);
    // Simulate a community switch: a teardown (unsubscribeAll) and a
    // (re)subscribe fired concurrently — both are void/fire-and-forget at the
    // App.svelte call sites, so without serialization unsubscribeAll's
    // subs.clear() could wipe an entry subscribeVisible just added.
    await Promise.all([svc.subscribeVisible([ownerA]), svc.unsubscribeAll()]);
    const tracked = new Set(
      (svc as unknown as { subs: Map<string, number> }).subs.values(),
    );
    // Invariant: every backend subscription that was created is either still
    // tracked by the frontend OR was explicitly unsubscribed — never orphaned
    // (live on the backend but absent from `subs`).
    for (const id of createdIds) {
      expect(tracked.has(id) || unsubscribed.includes(id)).toBe(true);
    }
  });
});
