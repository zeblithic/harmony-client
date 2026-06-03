// src/lib/group-call-banner-store.ts
import { writable } from 'svelte/store';

export interface GroupCallBannerEntry {
  callId: string;
  roster: { owner: string; device: string; muted: boolean }[];
}

/** spaceId(hex) → active-call banner entry. Updated by App.svelte from
 *  `group-call-presence-changed`. An empty roster clears the entry. When two
 *  callIds briefly coexist for one space (concurrent place), the LOWEST callId
 *  wins (matches the frontend reconciliation in the spec). */
function createGroupCallBannerStore() {
  const { subscribe, update } = writable<Record<string, GroupCallBannerEntry>>({});
  return {
    subscribe,
    apply(spaceId: string, callId: string, roster: GroupCallBannerEntry['roster']) {
      update((m) => {
        const next = { ...m };
        if (!roster || roster.length === 0) {
          // This call has no live participants. Only clear if the cleared call
          // is the one we're showing (don't wipe a different active call).
          if (next[spaceId]?.callId === callId) delete next[spaceId];
          return next;
        }
        const existing = next[spaceId];
        // Lowest-callId-wins: keep the existing entry if its callId is lower.
        if (existing && existing.callId !== callId && existing.callId < callId) {
          return next;
        }
        next[spaceId] = { callId, roster };
        return next;
      });
    },
    /** Drop a space's banner entry outright. Called on `unwatch_group_call`:
     *  once unwatched the app stops receiving presence updates for the space, so
     *  a call that ends while unwatched would otherwise leave a stale entry that
     *  `joinGroupCall` could drive a join into (a now-dead callId). */
    clear(spaceId: string) {
      update((m) => {
        if (!(spaceId in m)) return m;
        const next = { ...m };
        delete next[spaceId];
        return next;
      });
    },
  };
}

export const groupCallBanners = createGroupCallBannerStore();
