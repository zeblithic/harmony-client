/**
 * ZEB-949 Phase 2: tracks which freshly-joined communities are still doing
 * their first roster/channel sync, so the UI can show "Syncing…" instead of a
 * misleading empty state. Transient/in-memory: cleared on first synced content
 * (caller) or by a timeout safety-valve.
 *
 * A plain module (no Svelte runes) so it is unit-testable with vanilla vitest.
 * The consuming `.svelte` component bridges reactivity via the optional
 * `onChange` callback (fired on every mark/clear, including the timeout path).
 */
export interface InitialSyncTracker {
  markJoined(communityId: string): void;
  clear(communityId: string): void;
  isSyncing(communityId: string): boolean;
}

export function createInitialSyncTracker(
  timeoutMs = 10_000,
  onChange?: () => void,
): InitialSyncTracker {
  const syncing = new Set<string>();
  const timers = new Map<string, ReturnType<typeof setTimeout>>();

  function clear(communityId: string): void {
    const had = syncing.delete(communityId);
    const t = timers.get(communityId);
    if (t !== undefined) {
      clearTimeout(t);
      timers.delete(communityId);
    }
    if (had) onChange?.();
  }

  function markJoined(communityId: string): void {
    syncing.add(communityId);
    const existing = timers.get(communityId);
    if (existing !== undefined) clearTimeout(existing);
    timers.set(
      communityId,
      setTimeout(() => clear(communityId), timeoutMs),
    );
    onChange?.();
  }

  function isSyncing(communityId: string): boolean {
    return syncing.has(communityId);
  }

  return { markJoined, clear, isSyncing };
}
