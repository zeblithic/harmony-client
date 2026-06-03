// src/lib/group-dm-members-cache.ts
//
// Sync-readable cache of group-DM member owner-hex lists, backed by the async
// `get_group_dm_members` IPC (CRDT-authoritative). GroupCallSession.resolveMembers
// is synchronous and runs during the roster merge, so we warm this cache from
// App.svelte's group-call listeners (await ensureGroupMembers BEFORE forwarding
// the presence event) and read it synchronously in resolveMembers.

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

const cache = new Map<string, string[]>();
const inflight = new Map<string, { gen: number; promise: Promise<void> }>();
// Per-space generation token. An in-flight fetch captures the current
// generation at start and only writes its result back if the generation is
// still current — so an `invalidateGroupMembers` that fires DURING a fetch
// (membership change / identity switch) can't be overwritten by the now-stale
// result that resolves after it.
const generations = new Map<string, number>();

/** Synchronous read — returns [] until the cache is warmed. */
export function getCachedGroupMembers(spaceId: string): string[] {
  return cache.get(spaceId) ?? [];
}

/** Fetch + cache the member owner-hex list for a group DM (idempotent; dedups
 *  concurrent fetches). Safe to await before forwarding a presence event. */
export async function ensureGroupMembers(invoke: Invoke, spaceId: string): Promise<void> {
  if (cache.has(spaceId)) return;
  const existing = inflight.get(spaceId);
  // Only reuse an in-flight fetch if it was started in the CURRENT generation;
  // a fetch from before an invalidation is stale and must not be awaited as if
  // it reflected the post-invalidation membership.
  const currentGen = generations.get(spaceId) ?? 0;
  if (existing && existing.gen === currentGen) return existing.promise;
  const gen = currentGen;
  const promise = (async () => {
    try {
      const members = (await invoke('get_group_dm_members', { spaceId })) as string[];
      // Drop the result if an invalidation bumped the generation while we were
      // in flight — writing it back would resurrect stale membership.
      if ((generations.get(spaceId) ?? 0) === gen) cache.set(spaceId, members);
    } catch (e) {
      // Leave uncached on failure; a later event retries. Roster degrades to
      // live beacons only (no ringing rows) until it succeeds.
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[group-dm-members] warm failed:', msg);
    } finally {
      // Only clear OUR inflight slot — a newer-generation fetch may have already
      // replaced it.
      const slot = inflight.get(spaceId);
      if (slot && slot.gen === gen) inflight.delete(spaceId);
    }
  })();
  inflight.set(spaceId, { gen, promise });
  return promise;
}

/** Clear a cached entry (e.g. on membership change / identity switch). Bumps the
 *  per-space generation so any in-flight fetch's result is discarded instead of
 *  writing stale data back, and drops the in-flight slot so the next
 *  `ensureGroupMembers` starts a fresh fetch. */
export function invalidateGroupMembers(spaceId?: string): void {
  if (spaceId) {
    cache.delete(spaceId);
    generations.set(spaceId, (generations.get(spaceId) ?? 0) + 1);
    inflight.delete(spaceId);
  } else {
    cache.clear();
    for (const k of generations.keys()) generations.set(k, (generations.get(k) ?? 0) + 1);
    inflight.clear();
  }
}
