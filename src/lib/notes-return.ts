// ZEB-966: decision for the Messages footer button while the Notes space is
// open. Pre-fix, `appMode` was already 'messages' so the click was inert; the
// desired behavior is returning to the community the user was in before
// selecting Notes. App.svelte stashes that id in `selectNotes()` and applies
// the outcome (mirroring a left-nav community click).

export type MessagesReturnOutcome =
  | { action: 'restore-community'; communityId: string }
  | { action: 'none' };

export function resolveMessagesReturn(opts: {
  appMode: string;
  notesSelected: boolean;
  stashedCommunityId: string | null;
  communityExists: (id: string) => boolean;
}): MessagesReturnOutcome {
  const { appMode, notesSelected, stashedCommunityId, communityExists } = opts;
  // Only the already-in-messages no-op click restores. A cross-mode switch
  // (e.g. Vines → Messages) returns to whatever the messages view was —
  // Notes included — so it must not yank the user out of Notes.
  if (appMode !== 'messages' || !notesSelected) return { action: 'none' };
  // No prior community (fresh zero-community user), or the stash points at a
  // community since left/deleted: stay in Notes rather than render a void.
  if (stashedCommunityId === null || !communityExists(stashedCommunityId)) {
    return { action: 'none' };
  }
  return { action: 'restore-community', communityId: stashedCommunityId };
}
