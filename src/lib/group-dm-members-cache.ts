// src/lib/group-dm-members-cache.ts
//
// Sync-readable cache of group-DM member owner-hex lists, backed by the async
// `get_group_dm_members` IPC (CRDT-authoritative). GroupCallSession.resolveMembers
// is synchronous and runs during the roster merge, so we warm this cache from
// App.svelte's group-call listeners (await ensureGroupMembers BEFORE forwarding
// the presence event) and read it synchronously in resolveMembers.

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

const cache = new Map<string, string[]>();
// In-flight fetches keyed by spaceId. Each carries a monotonic `gen` token: a
// fetch only writes its result (and only clears its own slot) while it is STILL
// the current in-flight entry for the space. `invalidateGroupMembers` deletes
// the inflight entry, so any still-running fetch finds its gen no longer current
// and discards its write — closing the clear-all gap where a space with an
// in-flight fetch but no prior generation entry could still write stale data.
const inflight = new Map<string, { gen: number; promise: Promise<void> }>();
let counter = 0;

/** Synchronous read — returns [] until the cache is warmed. */
export function getCachedGroupMembers(spaceId: string): string[] {
  return cache.get(spaceId) ?? [];
}

/** Fetch + cache the member owner-hex list for a group DM (idempotent; dedups
 *  concurrent fetches). Safe to await before forwarding a presence event. */
export async function ensureGroupMembers(invoke: Invoke, spaceId: string): Promise<void> {
  if (cache.has(spaceId)) return;
  const existing = inflight.get(spaceId);
  if (existing) return existing.promise;
  const gen = ++counter;
  const promise = (async () => {
    try {
      const members = (await invoke('get_group_dm_members', { spaceId })) as string[];
      // Only write back if WE are still the current in-flight fetch. An
      // `invalidateGroupMembers` (membership change / identity switch) that
      // fired while we were in flight deletes our slot, so this guard discards
      // the now-stale result instead of resurrecting old membership.
      if (inflight.get(spaceId)?.gen === gen) cache.set(spaceId, members);
    } catch (e) {
      // Leave uncached on failure; a later event retries. Roster degrades to
      // live beacons only (no ringing rows) until it succeeds.
      const msg = e instanceof Error ? e.message : String(e);
      console.warn('[group-dm-members] warm failed:', msg);
    } finally {
      // Only clear OUR inflight slot — a newer fetch may have already replaced
      // it (and invalidate may have already removed it).
      if (inflight.get(spaceId)?.gen === gen) inflight.delete(spaceId);
    }
  })();
  inflight.set(spaceId, { gen, promise });
  return promise;
}

/** Clear a cached entry (e.g. on membership change / identity switch). Drops the
 *  in-flight slot so any running fetch finds its gen no longer current and
 *  discards its write, and so the next `ensureGroupMembers` starts a fresh
 *  fetch. Clear-all (no arg) does the same for every space — including spaces
 *  with an in-flight fetch but no cached entry yet. */
export function invalidateGroupMembers(spaceId?: string): void {
  if (spaceId) {
    cache.delete(spaceId);
    inflight.delete(spaceId);
  } else {
    cache.clear();
    inflight.clear();
  }
}
