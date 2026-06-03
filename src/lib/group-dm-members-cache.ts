// src/lib/group-dm-members-cache.ts
//
// Sync-readable cache of group-DM member owner-hex lists, backed by the async
// `get_group_dm_members` IPC (CRDT-authoritative). GroupCallSession.resolveMembers
// is synchronous and runs during the roster merge, so we warm this cache from
// App.svelte's group-call listeners (await ensureGroupMembers BEFORE forwarding
// the presence event) and read it synchronously in resolveMembers.

type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

const cache = new Map<string, string[]>();
const inflight = new Map<string, Promise<void>>();

/** Synchronous read — returns [] until the cache is warmed. */
export function getCachedGroupMembers(spaceId: string): string[] {
  return cache.get(spaceId) ?? [];
}

/** Fetch + cache the member owner-hex list for a group DM (idempotent; dedups
 *  concurrent fetches). Safe to await before forwarding a presence event. */
export async function ensureGroupMembers(invoke: Invoke, spaceId: string): Promise<void> {
  if (cache.has(spaceId)) return;
  const existing = inflight.get(spaceId);
  if (existing) return existing;
  const p = (async () => {
    try {
      const members = (await invoke('get_group_dm_members', { spaceId })) as string[];
      cache.set(spaceId, members);
    } catch {
      // Leave uncached on failure; a later event retries. Roster degrades to
      // live beacons only (no ringing rows) until it succeeds.
    } finally {
      inflight.delete(spaceId);
    }
  })();
  inflight.set(spaceId, p);
  return p;
}

/** Clear a cached entry (e.g. on membership change / identity switch). */
export function invalidateGroupMembers(spaceId?: string): void {
  if (spaceId) cache.delete(spaceId);
  else cache.clear();
}
