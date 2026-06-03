// src/lib/group-call-banner-store.ts
import { writable } from 'svelte/store';

export interface GroupCallBannerEntry {
  callId: string;
  roster: { owner: string; device: string; muted: boolean }[];
}

/** spaceId(hex) → active-call banner entry. Updated by App.svelte from
 *  `group-call-presence-changed`. An empty roster clears that call. When two
 *  callIds briefly coexist for one space (concurrent place), the LOWEST callId
 *  wins (matches the frontend reconciliation in the spec) — but we track ALL
 *  active calls internally so that when the surfaced (lowest) call ends, the
 *  banner falls back IMMEDIATELY to the next-lowest surviving call instead of
 *  going dark until that call's next presence event. */
function createGroupCallBannerStore() {
  const { subscribe, update } = writable<Record<string, GroupCallBannerEntry>>({});
  // Internal: all active calls per space (callId → roster). The PUBLIC store
  // (above) only ever holds the lowest-callId entry per space.
  const calls: Record<string, Record<string, GroupCallBannerEntry['roster']>> = {};

  /** Recompute the public entry for one space = the lowest-callId call whose
   *  roster is non-empty; remove the public entry if no calls remain. */
  function reconcile(
    next: Record<string, GroupCallBannerEntry>,
    spaceId: string,
  ): void {
    const inner = calls[spaceId];
    let lowest: string | null = null;
    if (inner) {
      for (const cid of Object.keys(inner)) {
        if (lowest === null || cid < lowest) lowest = cid;
      }
    }
    if (lowest === null) {
      delete next[spaceId];
    } else {
      next[spaceId] = { callId: lowest, roster: inner![lowest] };
    }
  }

  return {
    subscribe,
    apply(spaceId: string, callId: string, roster: GroupCallBannerEntry['roster']) {
      const inner = (calls[spaceId] ??= {});
      if (!roster || roster.length === 0) {
        // This call has no live participants — drop just THIS call, then
        // recompute (a higher concurrent call may now surface).
        delete inner[callId];
        if (Object.keys(inner).length === 0) delete calls[spaceId];
      } else {
        inner[callId] = roster;
      }
      update((m) => {
        const next = { ...m };
        reconcile(next, spaceId);
        return next;
      });
    },
    /** Drop a space's banner entry outright. Called on `unwatch_group_call`:
     *  once unwatched the app stops receiving presence updates for the space, so
     *  a call that ends while unwatched would otherwise leave a stale entry that
     *  `joinGroupCall` could drive a join into (a now-dead callId). Clears ALL
     *  tracked calls for the space, not just the surfaced one. */
    clear(spaceId: string) {
      delete calls[spaceId];
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
