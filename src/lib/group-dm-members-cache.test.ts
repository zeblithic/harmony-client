import { describe, it, expect, afterEach, vi } from 'vitest';
import {
  ensureGroupMembers,
  getCachedGroupMembers,
  invalidateGroupMembers,
} from './group-dm-members-cache';

afterEach(() => {
  invalidateGroupMembers();
  vi.restoreAllMocks();
});

/** A controllable invoke: resolves `get_group_dm_members` only when the returned
 *  `release` is called, so a test can interleave an invalidation mid-fetch. */
function deferredInvoke(members: string[]) {
  let release!: () => void;
  const gate = new Promise<void>((r) => {
    release = r;
  });
  const invoke = vi.fn(async (cmd: string) => {
    if (cmd === 'get_group_dm_members') {
      await gate;
      return members;
    }
    return undefined;
  });
  return { invoke, release };
}

describe('group-dm-members-cache', () => {
  it('warms and reads the cache synchronously', async () => {
    const invoke = vi.fn(async () => ['aaaa', 'bbbb']);
    expect(getCachedGroupMembers('space-1')).toEqual([]);
    await ensureGroupMembers(invoke, 'space-1');
    expect(getCachedGroupMembers('space-1')).toEqual(['aaaa', 'bbbb']);
  });

  it('dedups concurrent fetches for the same space', async () => {
    const { invoke, release } = deferredInvoke(['aaaa']);
    const p1 = ensureGroupMembers(invoke, 'space-1');
    const p2 = ensureGroupMembers(invoke, 'space-1');
    release();
    await Promise.all([p1, p2]);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(getCachedGroupMembers('space-1')).toEqual(['aaaa']);
  });

  it('clear-all DURING an in-flight fetch discards the stale write', async () => {
    const { invoke, release } = deferredInvoke(['stale']);
    const p = ensureGroupMembers(invoke, 'space-1');
    // Invalidate everything while the fetch is still in flight. This is the
    // round-2 gap: a space with an in-flight fetch but no prior cache entry must
    // still have its result discarded.
    invalidateGroupMembers();
    release();
    await p;
    // The stale result must NOT have been written back.
    expect(getCachedGroupMembers('space-1')).toEqual([]);
  });

  it('per-space invalidation DURING an in-flight fetch discards the stale write', async () => {
    const { invoke, release } = deferredInvoke(['stale']);
    const p = ensureGroupMembers(invoke, 'space-1');
    invalidateGroupMembers('space-1');
    release();
    await p;
    expect(getCachedGroupMembers('space-1')).toEqual([]);
  });

  it('a fresh fetch after invalidation writes the new membership', async () => {
    const first = deferredInvoke(['old']);
    const p1 = ensureGroupMembers(first.invoke, 'space-1');
    invalidateGroupMembers('space-1');
    // Start a brand-new fetch that returns the post-invalidation membership.
    const second = deferredInvoke(['new-a', 'new-b']);
    const p2 = ensureGroupMembers(second.invoke, 'space-1');
    first.release();
    second.release();
    await Promise.all([p1, p2]);
    // The stale fetch is discarded; the fresh one wins.
    expect(getCachedGroupMembers('space-1')).toEqual(['new-a', 'new-b']);
  });

  it('leaves the cache empty (and warns) on fetch failure', async () => {
    const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
    const invoke = vi.fn(async () => {
      throw new Error('boom');
    });
    await ensureGroupMembers(invoke, 'space-1');
    expect(getCachedGroupMembers('space-1')).toEqual([]);
    expect(warn).toHaveBeenCalled();
  });
});
